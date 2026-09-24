import { test } from "node:test";
import assert from "node:assert/strict";
import {
  FrameOrderer,
  KEY_REREQUEST_MS,
  MAX_HELD_FRAMES,
  REORDER_BUDGET_MS,
} from "./frameorder.js";
import type { WtFrame } from "./wtvideo.js";

function frame(frame_no: number, key = false, capture_us = frame_no * 16_667): WtFrame {
  return { frame_no, capture_us, key, payload: new Uint8Array([frame_no & 0xff]) };
}
const range = (from: number, to: number) => Array.from({ length: to - from + 1 }, (_, i) => from + i);
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
  // Frame 3 was dropped by the host. Everything after it piles up. The gate
  // waits out the repair budget (REORDER_BUDGET_MS of video, ~18 frames at
  // 60 fps) and then gives up rather than holding a growing backlog forever.
  const budgetFrames = Math.ceil((REORDER_BUDGET_MS * 1000) / 16_667) + 2;
  for (let n = 4; n <= 4 + budgetFrames; n++) o.accept(frame(n));
  assert.equal(o.needsResync, true, "must ask for a keyframe, not wait forever");
  assert.ok(
    o.held < budgetFrames,
    `must drop the undecodable backlog, still holding ${o.held}`,
  );
});

/**
 * The race, as a test. A hole repaired inside the client's own NACK ladder is
 * not a hole: the first repair round goes out 150 ms after the hole's first
 * fragment and lands an RTT later, so the gate must still be holding when the
 * frame finally shows up. Giving up at MAX_REORDER frames (133 ms at 60 fps)
 * beat the repair every time and turned every lost fragment into an IDR.
 */
test("a hole repaired within the repair budget costs no keyframe", () => {
  const o = new FrameOrderer();
  o.accept(frame(1, true));
  assert.deepEqual(nums(o.accept(frame(2))), [2]);
  // Frame 3 goes missing; 13 frames arrive behind it (~217 ms of video, which
  // the ladder's first round covers).
  for (let n = 4; n <= 16; n++) o.accept(frame(n));
  assert.equal(o.needsResync, false, "a repairable hole must not cost a re-key");
  assert.equal(o.held, 13, "the run waits for its hole");
  // The re-send lands: the whole run drains, in order, at once.
  assert.deepEqual(nums(o.accept(frame(3))), Array.from({ length: 14 }, (_, i) => i + 3));
  assert.equal(o.held, 0);
  assert.equal(o.needsResync, false);
});

test("a keyframe clears a pending resync", () => {
  const o = new FrameOrderer();
  o.accept(frame(1, true));
  for (let n = 3; n <= 3 + 21; n++) o.accept(frame(n));
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

/**
 * The measured stall (audit §2.1). Waiting for an IDR the client already asked
 * for, the gate used to clear its hold every 300 ms of video - destroying the
 * deltas that raced ahead of the (large, late) IDR, so the IDR that finally
 * landed anchored nothing, the hole behind it spent the budget again, and one
 * lost frame became seconds of back-to-back IDR requests.
 */
test("an unanchored run is held past the repair budget and drains when the IDR lands", () => {
  const o = new FrameOrderer();
  o.accept(frame(1, true));
  for (let n = 2; n <= 5; n++) o.accept(frame(n));
  o.reset(); // a reference gap: waiting for the IDR the decoder asked for
  // The old sequence keeps arriving until the host acts on the request...
  for (let n = 6; n <= 15; n++) o.accept(frame(n));
  // ...then the requested IDR is frame 40, and its 25 deltas (~417 ms of
  // video, well past the budget) outrun it.
  for (let n = 41; n <= 65; n++) {
    assert.deepEqual(nums(o.accept(frame(n))), []);
    assert.equal(o.needsResync, false, `frame ${n}: the wait for a requested IDR is not a hole`);
  }
  assert.ok(o.held > 18, `the run must be held, not cleared: held ${o.held}`);
  assert.deepEqual(nums(o.accept(frame(40, true))), range(40, 65), "the IDR releases the whole run");
  assert.equal(o.held, 0, "the old sequence behind the IDR is pruned, not released");
});

test("an unanchored hold is bounded by evicting the oldest capture, not the run", () => {
  const o = new FrameOrderer();
  for (let n = 1; n <= 36; n++) o.accept(frame(n)); // stale deltas from before the IDR
  for (let n = 100; n <= 163; n++) o.accept(frame(n)); // the run racing ahead of it
  assert.equal(o.held, MAX_HELD_FRAMES, "memory stays bounded");
  assert.equal(o.needsResync, false);
  assert.deepEqual(nums(o.accept(frame(99, true))), range(99, 163), "the newest run survived");
});

test("while unanchored a keyframe is re-requested once per two budget windows, not per frame", () => {
  const o = new FrameOrderer();
  o.accept(frame(1));
  assert.equal(o.keyframeDue, true, "the first unanchored arrival asks at once");
  o.keyframeRequested();
  const step = 16_667;
  const windowFrames = Math.ceil((KEY_REREQUEST_MS * 1000) / step);
  let dueAt: number[] = [];
  for (let n = 2; n <= 1 + 3 * windowFrames; n++) {
    o.accept(frame(n));
    if (o.keyframeDue) {
      dueAt.push(n);
      o.keyframeRequested();
    }
  }
  assert.equal(dueAt.length, 3, `due at ${dueAt.join(",")}`);
  assert.ok(dueAt[1]! - dueAt[0]! >= windowFrames - 1, "spaced by the re-request window");
  // A request the page throttle refused is not lost: it stays due.
  dueAt = [];
  for (let n = 2000; n < 2000 + windowFrames + 3; n++) o.accept(frame(n));
  assert.equal(o.keyframeDue, true);
  o.accept(frame(2100));
  assert.equal(o.keyframeDue, true, "still due until the caller reports a request sent");
  assert.deepEqual(nums(o.accept(frame(2101, true))), [2101]);
  assert.equal(o.keyframeDue, false, "an anchor answers the request");
});

/**
 * Audit §3.5/§3.6: the host sends a repair IDR on the stream AND as datagrams,
 * so the page can see it twice. Anchoring on the second copy rewound the cursor
 * behind frames already decoded - a hole nothing can fill, which spent the
 * budget and asked for yet another IDR.
 */
test("a keyframe at or behind the last released frame is a replay, never an anchor", () => {
  const o = new FrameOrderer();
  assert.deepEqual(nums(o.accept(frame(1, true))), [1]);
  for (let n = 2; n <= 5; n++) assert.deepEqual(nums(o.accept(frame(n))), [n]);
  assert.deepEqual(nums(o.accept(frame(1, true))), [], "the second copy of the IDR");
  assert.deepEqual(nums(o.accept(frame(4, true))), [], "a stale IDR behind the cursor");
  assert.deepEqual(nums(o.accept(frame(6))), [6], "the sequence carries on");
  assert.equal(o.needsResync, false, "a replay must not cost a keyframe");
  assert.deepEqual(nums(o.accept(frame(90, true))), [90], "a fresh IDR still anchors");
});

test("a replayed keyframe cannot anchor a sequence that is waiting for a fresh one", () => {
  const o = new FrameOrderer();
  o.accept(frame(10, true));
  for (let n = 11; n <= 20; n++) o.accept(frame(n));
  o.reset();
  assert.deepEqual(nums(o.accept(frame(10, true))), [], "the late datagram copy of IDR 10");
  assert.equal(o.needsKeyframe, true, "still waiting for the requested IDR");
  assert.deepEqual(nums(o.accept(frame(30, true))), [30]);
});

test("a host whose frame numbering restarts is a new sequence, not a replay", () => {
  const o = new FrameOrderer();
  o.accept(frame(700, true));
  for (let n = 701; n <= 705; n++) o.accept(frame(n));
  // Numbered behind the cursor, but captured after everything released.
  const later = 706 * 16_667;
  assert.deepEqual(nums(o.accept(frame(0, true, later))), [0]);
  assert.deepEqual(nums(o.accept(frame(1, false, later + 16_667))), [1]);
});

/**
 * The host's capture_us is the pipeline PTS: it restarts near zero with each
 * pipeline while frame numbers carry on transport-wide. A new pipeline must
 * anchor and decode, and the re-request clock must follow it onto the new PTS.
 */
test("a pipeline whose capture clock restarts still anchors and re-requests", () => {
  const o = new FrameOrderer();
  o.accept(frame(700, true));
  for (let n = 701; n <= 705; n++) o.accept(frame(n));
  o.reset();
  o.accept(frame(706, false, 1_000));
  assert.equal(o.keyframeDue, true);
  o.keyframeRequested();
  for (let n = 707; n <= 760; n++) o.accept(frame(n, false, (n - 706) * 16_667));
  assert.equal(o.keyframeDue, true, "the re-request falls due on the new clock");
  assert.deepEqual(nums(o.accept(frame(761, true, 55 * 16_667))), [761]);
  assert.deepEqual(nums(o.accept(frame(762, false, 56 * 16_667))), [762]);
});

test("a reconfigured stream forgets the replay guard", () => {
  const o = new FrameOrderer();
  o.accept(frame(700, true));
  o.accept(frame(701));
  o.reset(true);
  assert.deepEqual(nums(o.accept(frame(0, true))), [0], "numbering and PTS both restarted");
});

test("the repair wait scales with RTT, never past the fixed budget", async () => {
  const { repairBudgetMs } = await import("./frameorder.js");
  assert.equal(repairBudgetMs(0), REORDER_BUDGET_MS, "unknown RTT keeps the full budget");
  assert.ok(repairBudgetMs(5) < REORDER_BUDGET_MS && repairBudgetMs(5) >= 180, `LAN ${repairBudgetMs(5)}`);
  assert.equal(repairBudgetMs(60), REORDER_BUDGET_MS, "a WAN route keeps the ceiling");
});
