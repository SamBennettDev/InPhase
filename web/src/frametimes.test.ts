import { test } from "node:test";
import assert from "node:assert/strict";
import { FrameTimeline } from "./frametimes.js";

/** Feed one frame through every stage. */
function frame(t: FrameTimeline, us: number, arrive: number, decoded: number, drawn: number, inRaf: number | null = null) {
  t.arrived(us, arrive, 10);
  t.decoded(us, decoded);
  t.drawn(us, drawn, inRaf);
}

test("a frame drawn between refreshes is shown by the next one", () => {
  const t = new FrameTimeline();
  frame(t, 1, 100, 103, 104);
  assert.equal(t.samples().length, 0, "not on screen until a refresh");
  t.refresh(108);
  const [s] = t.samples();
  assert.deepEqual(s, { shownMs: 108, intervalMs: 0, netMs: 10, decodeMs: 3, waitMs: 1, showMs: 4 });
});

test("a frame drawn inside a refresh callback is shown by that refresh", () => {
  const t = new FrameTimeline();
  t.arrived(1, 100, null);
  t.decoded(1, 101);
  t.drawn(1, 109, 108);
  assert.equal(t.samples()[0]!.shownMs, 109);
  assert.equal(t.samples()[0]!.netMs, null);
});

test("intervals are between frames that reached the screen", () => {
  const t = new FrameTimeline();
  for (let i = 0; i < 5; i++) {
    frame(t, i, i * 16, i * 16 + 2, i * 16 + 3);
    t.refresh(i * 16 + 8);
  }
  assert.deepEqual(t.samples().map((s) => s.intervalMs), [0, 16, 16, 16, 16]);
});

test("two draws before one refresh: only the second is shown", () => {
  const t = new FrameTimeline();
  frame(t, 1, 0, 1, 2);
  frame(t, 2, 3, 4, 5);
  t.refresh(8);
  assert.equal(t.samples().length, 1);
  assert.equal(t.summary(60).overdrawn, 1);
});

test("summary: percentiles and jank against the slower of stream and display", () => {
  const t = new FrameTimeline();
  let now = 0;
  // 60 Hz display: a refresh every 16.7 ms; one frame per refresh, and one
  // hitch that skips three refreshes.
  for (let i = 0; i < 100; i++) {
    now += i === 50 ? 16.7 * 4 : 16.7;
    frame(t, i, now - 6, now - 4, now - 3);
    t.refresh(now);
  }
  const s = t.summary(120); // 120 fps stream on the 60 Hz panel
  assert.ok(Math.abs(s.refreshMs - 16.7) < 0.01);
  assert.ok(Math.abs(s.p50 - 16.7) < 0.01, "steady 16.7 is the panel, not stutter");
  assert.equal(s.jank, 1);
  assert.ok(Math.abs(s.max - 66.8) < 0.01);
  assert.equal(s.netMs, 10);
});

test("a summary since a time covers only later frames", () => {
  const t = new FrameTimeline();
  for (let i = 0; i < 10; i++) {
    frame(t, i, i * 10, i * 10, i * 10);
    t.refresh(i * 10 + (i >= 8 ? 35 : 5)); // frame 8 shows 30 ms late
  }
  const s = t.summary(100, 60); // frames shown at 65, 75, 115, 125
  assert.equal(s.frames, 4);
  assert.equal(s.max, 40);
});

test("an early refresh callback does not make a 60 Hz page look like 120 Hz", () => {
  // iPhone Safari at 60 Hz, a 120 fps stream: one frame shown per refresh,
  // and now and then a callback lands a few ms after the last.
  const t = new FrameTimeline();
  let now = 0;
  for (let i = 0; i < 120; i++) {
    now += i % 30 === 7 ? 5 : 16.7;
    frame(t, i, now - 8, now - 3, now - 1);
    t.refresh(now);
  }
  const s = t.summary(120);
  assert.ok(Math.abs(t.targetMs(120) - 16.7) < 0.01);
  assert.equal(s.jank, 0, "steady 60 Hz is not stutter");
});

test("the window keeps the newest frames", () => {
  const t = new FrameTimeline(10);
  for (let i = 0; i < 25; i++) {
    frame(t, i, i * 10, i * 10, i * 10);
    t.refresh(i * 10 + 5);
  }
  assert.equal(t.samples().length, 10);
  assert.equal(t.samples()[0]!.shownMs, 155);
});

test("discarded and unknown frames leave no samples", () => {
  const t = new FrameTimeline();
  t.arrived(1, 0, 5);
  t.discard(1);
  t.drawn(1, 2);
  t.drawn(99, 3);
  t.refresh(8);
  assert.equal(t.samples().length, 0);
});
