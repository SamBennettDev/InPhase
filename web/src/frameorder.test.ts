import { test } from "node:test";
import assert from "node:assert/strict";
import { FrameOrderer, MAX_REORDER } from "./frameorder.js";
import type { WtFrame } from "./wtvideo.js";

function frame(frame_no: number, key = false): WtFrame {
  return { frame_no, capture_us: frame_no * 16_667, key, payload: new Uint8Array([frame_no & 0xff]) };
}
const nums = (fs: WtFrame[]) => fs.map((f) => f.frame_no);

test("an in-order stream passes straight through and holds nothing", () => {
  const o = new FrameOrderer();
  assert.deepEqual(nums(o.accept(frame(10, true))), [10]);
  for (let n = 11; n < 40; n++) {
    assert.deepEqual(nums(o.accept(frame(n))), [n]);
  }
  assert.equal(o.held, 0, "steady state must not buffer");
});

/**
 * The bug this class exists for. Streams complete independently, so a 400 KB
 * keyframe finishes *after* the small deltas queued behind it. The old gate
 * moved its cursor to the deltas and then dropped the keyframe as stale — and
 * since a keyframe is always the largest thing on the wire it lost that race
 * every time, so the client asked for a keyframe forever and decoded nothing.
 */
test("a keyframe arriving after the deltas behind it still anchors", () => {
  const o = new FrameOrderer();
  // The deltas land first and wait.
  assert.deepEqual(nums(o.accept(frame(11))), []);
  assert.deepEqual(nums(o.accept(frame(12))), []);
  assert.deepEqual(nums(o.accept(frame(13))), []);
  // The keyframe finally finishes transferring - and releases all of them.
  assert.deepEqual(nums(o.accept(frame(10, true))), [10, 11, 12, 13]);
  assert.equal(o.held, 0);
});

test("frames older than the anchoring keyframe are discarded", () => {
  const o = new FrameOrderer();
  o.accept(frame(5));
  o.accept(frame(6));
  // A keyframe at 10 makes 5 and 6 undecodable - they must not be emitted.
  assert.deepEqual(nums(o.accept(frame(10, true))), [10]);
  assert.deepEqual(nums(o.accept(frame(11))), [11]);
});

test("out-of-order deltas are reassembled in order", () => {
  const o = new FrameOrderer();
  o.accept(frame(1, true));
  assert.deepEqual(nums(o.accept(frame(4))), []);
  assert.deepEqual(nums(o.accept(frame(3))), []);
  assert.deepEqual(nums(o.accept(frame(2))), [2, 3, 4], "the run drains at once");
});

test("nothing decodes before a keyframe anchors", () => {
  const o = new FrameOrderer();
  assert.equal(o.needsKeyframe, true);
  for (let n = 1; n < 5; n++) assert.deepEqual(nums(o.accept(frame(n))), []);
  assert.equal(o.needsKeyframe, true);
});

/**
 * The freeze. A frame dropped by the host's leaky queue never arrives, and with
 * an infinite GOP everything behind it is undecodable. Waiting is pure latency:
 * give up quickly rather than holding a second of video hostage.
 */
test("a hole that never fills gives up instead of stalling", () => {
  const o = new FrameOrderer();
  o.accept(frame(1, true));
  assert.deepEqual(nums(o.accept(frame(2))), [2]);
  // Frame 3 was dropped by the host. Everything after it piles up.
  for (let n = 4; n <= 4 + MAX_REORDER; n++) o.accept(frame(n));
  assert.equal(o.needsResync, true, "must ask for a keyframe, not wait forever");
  assert.ok(
    o.held < MAX_REORDER,
    `must drop the undecodable backlog, still holding ${o.held}`,
  );
});

test("a keyframe clears a pending resync", () => {
  const o = new FrameOrderer();
  o.accept(frame(1, true));
  for (let n = 3; n <= 3 + MAX_REORDER + 1; n++) o.accept(frame(n));
  assert.equal(o.needsResync, true);
  assert.deepEqual(nums(o.accept(frame(100, true))), [100]);
  assert.equal(o.needsResync, false, "the keyframe is the repair");
  assert.deepEqual(nums(o.accept(frame(101))), [101]);
});

test("duplicate and stale frames are ignored", () => {
  const o = new FrameOrderer();
  o.accept(frame(10, true));
  assert.deepEqual(nums(o.accept(frame(11))), [11]);
  assert.deepEqual(nums(o.accept(frame(11))), [], "duplicate");
  assert.deepEqual(nums(o.accept(frame(9))), [], "stale");
  assert.deepEqual(nums(o.accept(frame(12))), [12]);
});

test("frame numbers that wrap are handled", () => {
  const o = new FrameOrderer();
  const near = 0xfffffffe;
  assert.deepEqual(nums(o.accept(frame(near, true))), [near]);
  assert.deepEqual(nums(o.accept(frame(0xffffffff))), [0xffffffff]);
  assert.deepEqual(nums(o.accept(frame(0))), [0], "wraps forward, not stale");
  assert.deepEqual(nums(o.accept(frame(1))), [1]);
});

test("reset waits for a fresh keyframe", () => {
  const o = new FrameOrderer();
  o.accept(frame(1, true));
  o.accept(frame(2));
  o.reset();
  assert.equal(o.needsKeyframe, true);
  assert.deepEqual(nums(o.accept(frame(3))), [], "no anchor, nothing decodes");
  assert.deepEqual(nums(o.accept(frame(4, true))), [4]);
});

/**
 * Replay of a real arrival trace captured from the host by wt_probe.
 *
 * The first four frames are 685-687 from the *previous* pipeline, still being
 * delivered when the session started; then numbering restarts at 0 with the
 * keyframe. Those stragglers are numerically far *ahead* of the new anchor, so
 * they sat in the hold buffer forever, and the first genuine reorder tipped it
 * over the limit: clear, resync, ask for a keyframe, repeat. The client
 * received 6.8 Mbps at 0% loss and decoded nothing.
 */
test("a real arrival trace decodes, stale frames from a previous pipeline and all", async () => {
  const { RealArrivals } = await import("./frameorder.arrivals.gen.js");
  const o = new FrameOrderer();
  const decoded: number[] = [];
  for (const a of RealArrivals) {
    for (const f of o.accept(frame(a.n, a.key))) decoded.push(f.frame_no);
    assert.equal(o.needsResync, false, `resync demanded after frame ${a.n}`);
  }
  // Everything from the anchoring keyframe onward must decode, in order. The
  // stragglers ahead of it belong to the previous pipeline and must not appear.
  const keyAt = RealArrivals.findIndex((a) => a.key);
  const expected = [...new Set(RealArrivals.slice(keyAt).map((a) => a.n))].sort((x, y) => x - y);
  assert.deepEqual(decoded, expected, "every frame after the anchor decodes in order");
  const stale = RealArrivals.slice(0, keyAt).map((a) => a.n);
  assert.ok(
    stale.length > 0 && !decoded.some((n) => stale.includes(n)),
    "the previous pipeline's frames must not reach the decoder",
  );
});

test("a frame absurdly far ahead of the anchor is not a reorder", () => {
  const o = new FrameOrderer();
  o.accept(frame(10, true));
  assert.deepEqual(nums(o.accept(frame(11))), [11]);
  // Not out-of-order delivery - a different sequence entirely.
  assert.deepEqual(nums(o.accept(frame(99_999))), [], "dropped, not held");
  assert.equal(o.held, 0, "must not poison the hold buffer");
  assert.deepEqual(nums(o.accept(frame(12))), [12], "the real stream continues");
});
