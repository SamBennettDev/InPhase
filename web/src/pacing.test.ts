import { test } from "node:test";
import assert from "node:assert/strict";
import { MAX_PACED_FRAMES, MAX_PACING_DELAY_MS, PacedQueue, PlayoutClock } from "./pacing.js";

// The PC's clock runs 1e9 ms ahead of the page's; nothing may depend on it.
const OFFSET_MS = 1e9;
const capUs = (clientMs: number) => (clientMs + OFFSET_MS) * 1000;

test("a steady stream gets no added delay", () => {
  const c = new PlayoutClock();
  for (let i = 0; i < 100; i++) c.observe(capUs(i * 16.7), i * 16.7 + 20);
  assert.ok(c.delayMs <= 1.01, `delay ${c.delayMs}`);
  // Due = capture + the fastest transit (20 ms) + the delay.
  assert.ok(Math.abs(c.dueMs(capUs(2000)) - (2020 + c.delayMs)) < 1e-6);
});

test("jitter sizes the delay to the slow tail, not the worst frame", () => {
  const c = new PlayoutClock();
  for (let i = 0; i < 240; i++) {
    // Every 10th frame 15 ms late, one frame 200 ms late.
    const late = i === 100 ? 200 : i % 10 === 0 ? 15 : 0;
    c.observe(capUs(i * 8.3), i * 8.3 + 20 + late);
  }
  assert.ok(c.delayMs >= 15 && c.delayMs <= 17, `delay ${c.delayMs}`);
});

test("the delay rises at once and comes down slowly", () => {
  const c = new PlayoutClock();
  let t = 0;
  for (let i = 0; i < 240; i++, t += 10) c.observe(capUs(t), t + 20 + (i % 5 === 0 ? 30 : 0));
  const high = c.delayMs;
  assert.ok(high >= 30, `delay ${high}`);
  // The route calms. While the bumpy frames are still in the window the
  // delay holds; once they age out it falls ~4 ms a second, not to 0.
  for (let i = 0; i < 100; i++, t += 10) c.observe(capUs(t), t + 20);
  assert.equal(c.delayMs, high, "the window still holds the jitter");
  for (let i = 0; i < 240; i++, t += 10) c.observe(capUs(t), t + 20);
  assert.ok(c.delayMs < high - 2 && c.delayMs > high - 12, `${high} -> ${c.delayMs}`);
});

test("the delay never exceeds the cap", () => {
  const c = new PlayoutClock();
  for (let i = 0; i < 240; i++) c.observe(capUs(i * 10), i * 10 + (i % 2 ? 500 : 0));
  assert.equal(c.delayMs, MAX_PACING_DELAY_MS);
});

test("the queue releases frames in order once due", () => {
  const q = new PacedQueue<string>();
  q.push("a", 10);
  q.push("b", 20);
  q.push("c", 40);
  assert.equal(q.nextDue, 10);
  assert.deepEqual(q.take(5), []);
  assert.deepEqual(q.take(25), ["a", "b"]);
  assert.deepEqual(q.take(40), ["c"]);
  assert.equal(q.nextDue, null);
});

test("a frame due later holds back the ones behind it: order wins", () => {
  const q = new PacedQueue<string>();
  q.push("a", 30); // queued when the delay was higher
  q.push("b", 20);
  assert.deepEqual(q.take(25), []);
  assert.deepEqual(q.take(30), ["a", "b"]);
});

test("a full queue releases its oldest early, never drops", () => {
  const q = new PacedQueue<number>();
  const early: number[] = [];
  for (let i = 0; i < MAX_PACED_FRAMES + 3; i++) early.push(...q.push(i, 1000 + i));
  assert.equal(q.size, MAX_PACED_FRAMES);
  assert.deepEqual(early, [0, 1, 2]);
  assert.equal(q.clear().length, MAX_PACED_FRAMES);
});
