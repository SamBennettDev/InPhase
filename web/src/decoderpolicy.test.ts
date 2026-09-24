import { test } from "node:test";
import assert from "node:assert/strict";
import {
  KeyframeThrottle,
  DecoderRestartPolicy,
  EARLY_FAILURE_MS,
  MAX_DECODER_REBUILDS,
  MAX_DECODE_QUEUE,
  BEHIND_PERSISTENCE,
  behindIsSustained,
  decoderIsBehind,
  FREEZE_GAP_MS,
  KEYFRAME_REQUEST_INTERVAL_MS,
  sameDecoderConfig,
  shouldCountFreeze,
} from "./decoderpolicy.js";

test("the first keyframe request is allowed, the next is not", () => {
  const t = new KeyframeThrottle(500);
  assert.equal(t.allow(1000), true);
  assert.equal(t.allow(1100), false);
  assert.equal(t.allow(1499), false);
  assert.equal(t.allow(1500), true, "the window has passed");
});

/**
 * The bug. `referenceGap()` stamped the shared field with `Date.now()` and
 * `requestKeyframe()` compared it against `performance.now()`. A decode-error
 * burst then sent one IDR request per error - the host re-encodes a full
 * keyframe for each - because no comparison between the two clocks means
 * anything.
 */
test("a burst of gaps sends one request, not one per gap", () => {
  const t = new KeyframeThrottle(500);
  let sent = 0;
  // 200 errors inside 100 ms, as captured in the browser.
  for (let i = 0; i < 200; i++) if (t.allow(5000 + i * 0.5)) sent++;
  assert.equal(sent, 1, `a 100 ms burst must send one request, sent ${sent}`);
});

test("the throttle uses one clock, so a huge timestamp cannot unlock it", () => {
  const t = new KeyframeThrottle(500);
  assert.equal(t.allow(1_759_000_000_000), true, "a Date.now()-scale first call");
  assert.equal(t.allow(1_759_000_000_100), false, "100 ms later, still throttled");
});

test("reset lets the next request through immediately", () => {
  const t = new KeyframeThrottle(500);
  assert.equal(t.allow(1000), true);
  assert.equal(t.allow(1100), false);
  t.reset();
  assert.equal(t.allow(1100), true, "a fresh sequence may ask at once");
});

test("a failing decoder is rebuilt a bounded number of times, then gives up", () => {
  const p = new DecoderRestartPolicy();
  for (let i = 0; i < MAX_DECODER_REBUILDS; i++) {
    assert.equal(p.onError(1000 + i), "rebuild", `attempt ${i + 1} should rebuild`);
  }
  assert.equal(p.onError(1100), "give-up", "an unusable config must end in a named failure");
});

/**
 * The freeze, as a policy question. Unbounded rebuilding turns one bad config
 * into a synchronous error/configure loop that wedges the main thread - the
 * page stops taking input entirely.
 */
test("a tight error storm cannot rebuild forever", () => {
  const p = new DecoderRestartPolicy();
  let rebuilds = 0;
  for (let i = 0; i < 500; i++) if (p.onError(2000 + i * 0.2) === "rebuild") rebuilds++;
  assert.equal(rebuilds, MAX_DECODER_REBUILDS, `rebuilt ${rebuilds} times in 100 ms`);
});

test("a decoder that recovers gets its full budget back", () => {
  const p = new DecoderRestartPolicy();
  p.onError(1000);
  p.onError(1001);
  p.onProgress(); // a frame decoded: whatever it was, it passed
  assert.equal(p.failureCount, 0);
  for (let i = 0; i < MAX_DECODER_REBUILDS; i++) {
    assert.equal(p.onError(2000 + i), "rebuild");
  }
});

test("failures spread across time are recovery, not an unusable config", () => {
  const p = new DecoderRestartPolicy(5, 10_000);
  // One error every 30 s: a lossy route re-keying, which must never give up.
  for (let i = 0; i < 50; i++) {
    assert.equal(p.onError(i * 30_000), "rebuild", `isolated error ${i} must still rebuild`);
  }
});

test("a decoder inside its queue budget is fed normally", () => {
  for (const q of [0, 1, 5, MAX_DECODE_QUEUE]) {
    assert.equal(decoderIsBehind(q), false, `queue ${q} is not behind`);
  }
});

/**
 * The freeze, measured in a browser: frames received held at 60/s while
 * decoded fell 64, 58, 42, 35, 26, 15, 1 and the canvas stopped changing.
 * Nothing consulted `decodeQueueSize`, so nothing could see it.
 */
test("a decoder past its queue budget is skipped forward, not fed", () => {
  assert.equal(decoderIsBehind(MAX_DECODE_QUEUE + 1), true);
  assert.equal(decoderIsBehind(60), true, "a second of backlog is not recoverable");
});

/**
 * A repaired hole releases a whole held run at once (infinite GOP: every frame
 * of it must decode, in order), so the queue crosses the limit on a client that
 * is keeping up perfectly. Only a queue that STAYS over the limit is a decoder
 * that has fallen behind — tripping on the first sample turned every repaired
 * hole into another keyframe request, drop, and codec reset.
 */
test("a transient queue is not a decoder that has fallen behind", () => {
  assert.equal(behindIsSustained(1), false, "one release burst is not a backlog");
  assert.equal(behindIsSustained(BEHIND_PERSISTENCE), false, "still draining");
  assert.equal(behindIsSustained(BEHIND_PERSISTENCE + 1), true, "this one is stuck");
});

/**
 * Audit §3.1: the page asked every 500 ms while the worker and the host each
 * gate at 1000 ms, so half the asks were refused and the client believed a
 * repair was on its way when none was.
 */
test("the default keyframe cadence never outruns the 1000 ms gates behind it", () => {
  assert.ok(KEYFRAME_REQUEST_INTERVAL_MS >= 1000);
  const t = new KeyframeThrottle();
  assert.equal(t.allow(0), true);
  assert.equal(t.allow(999), false, "the host would refuse this one");
  assert.equal(t.allow(KEYFRAME_REQUEST_INTERVAL_MS), true);
});

test("a hidden page's present gap is not a freeze", () => {
  assert.equal(shouldCountFreeze(FREEZE_GAP_MS + 1, false), true);
  assert.equal(shouldCountFreeze(FREEZE_GAP_MS, false), false, "pacing, not a stall");
  assert.equal(shouldCountFreeze(10_000, true), false, "a tab switch is not a freeze");
});

test("a duplicate video_config is recognised, description compared by bytes", () => {
  const base = { codec: "hvc1.1.6.L153.B0", width: 2560, height: 1440 };
  assert.equal(sameDecoderConfig(null, base), false, "the first config always applies");
  assert.equal(sameDecoderConfig(base, { ...base }), true);
  assert.equal(
    sameDecoderConfig(
      { ...base, description: new Uint8Array([1, 2, 3]) },
      { ...base, description: new Uint8Array([1, 2, 3]) },
    ),
    true,
    "a re-decoded identical description is the same config",
  );
  assert.equal(
    sameDecoderConfig({ ...base, description: new Uint8Array([1, 2, 3]) }, { ...base, description: new Uint8Array([1, 2, 4]) }),
    false,
  );
  assert.equal(sameDecoderConfig(base, { ...base, description: new Uint8Array([1]) }), false, "a description arriving is a change");
  assert.equal(sameDecoderConfig(base, { ...base, width: 1920 }), false);
});

/**
 * Chromium + VA-API (AMD Renoir): the hardware decoder errored on the host's
 * H.264 and every rebuild answered "Unsupported configuration" - five rebuilds
 * in ~15 ms, then "this browser cannot decode it", from a browser that decodes
 * the stream fine in software.
 */
test("a hardware decoder that fails before its first picture falls back to software once", () => {
  const p = new DecoderRestartPolicy();
  assert.equal(p.onError(1000, true), "fallback-software", "the driver, not the stream");
  // The caller is on software now and says so: normal budget from here.
  for (let i = 0; i < MAX_DECODER_REBUILDS; i++) {
    assert.equal(p.onError(1001 + i, false), "rebuild", `software attempt ${i + 1}`);
  }
  assert.equal(p.onError(1100, false), "give-up", "software failing too is a named failure");
});

test("a burst on a decoder that had been working tries software before giving up", () => {
  const p = new DecoderRestartPolicy();
  p.onProgress(0);
  const t0 = EARLY_FAILURE_MS + 60_000;
  for (let i = 0; i < MAX_DECODER_REBUILDS; i++) {
    assert.equal(p.onError(t0 + i, true), "rebuild", "an isolated error keeps the hardware path");
  }
  assert.equal(p.onError(t0 + 10, true), "fallback-software", "the burst that would give up switches instead");
});

test("an error soon after the first picture still counts as early", () => {
  const p = new DecoderRestartPolicy();
  p.onProgress(1000);
  assert.equal(p.onError(1000 + EARLY_FAILURE_MS - 1, true), "fallback-software");
});

test("step-down drops the frame rate to 60 before the resolution", async () => {
  const { nextStepDown } = await import("./decoderpolicy.js");
  assert.deepEqual(nextStepDown({ width: 2560, height: 1440, fps: 120 }), { width: 2560, height: 1440, fps: 60 });
  assert.deepEqual(nextStepDown({ width: 2560, height: 1440, fps: 60 }), { width: 1920, height: 1080, fps: 60 });
  assert.deepEqual(nextStepDown({ width: 3840, height: 2160, fps: 60 }), { width: 2560, height: 1440, fps: 60 });
  assert.deepEqual(nextStepDown({ width: 1920, height: 1080, fps: 30 }), { width: 1280, height: 720, fps: 30 });
  assert.equal(nextStepDown({ width: 1280, height: 720, fps: 60 }), null, "720p60 is the floor");
});

test("a working decoder that fails twice steps down; an early failure still falls back to software", async () => {
  const { DecoderRestartPolicy } = await import("./decoderpolicy.js");
  const p = new DecoderRestartPolicy();
  p.onProgress(1000); // decoding
  assert.equal(p.onError(20_000, true, true), "rebuild", "one failure of a working decoder: rebuild");
  p.onProgress(20_100);
  assert.equal(p.onError(25_000, true, true), "step-down", "it failed again within a minute: overloaded");

  const q = new DecoderRestartPolicy();
  assert.equal(q.onError(0, true, true), "fallback-software", "never decoded: incompatible, not overloaded");

  const r = new DecoderRestartPolicy();
  r.onProgress(0);
  assert.equal(r.onError(20_000, true, true), "rebuild");
  r.onProgress(20_100);
  assert.equal(r.onError(90_000, true, true), "rebuild", "failures a minute apart are a route recovering");

  const floor = new DecoderRestartPolicy();
  floor.onProgress(0);
  floor.onError(20_000, false, false);
  floor.onProgress(20_100);
  assert.equal(floor.onError(25_000, false, false), "rebuild", "no step-down left: the old policy applies");
});
