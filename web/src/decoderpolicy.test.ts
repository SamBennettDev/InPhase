import { test } from "node:test";
import assert from "node:assert/strict";
import {
  KeyframeThrottle,
  DecoderRestartPolicy,
  MAX_DECODER_REBUILDS,
  MAX_DECODE_QUEUE,
  decoderIsBehind,
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
