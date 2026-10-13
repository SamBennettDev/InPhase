import assert from "node:assert/strict";
import test from "node:test";
import { PresentGate, WtDecoder, glassAgeMs } from "./wtdecoder.js";
import { FrameOrderer } from "./frameorder.js";
import { FrameTimeline } from "./frametimes.js";
import { PacedQueue, PlayoutClock } from "./pacing.js";



test("present gate: newest frame wins, stale conversions discard", () => {
  const g = new PresentGate();
  const a = g.begin();
  assert.equal(g.settle(a), "draw", "the only conversion draws");
  const b = g.begin();
  const c = g.begin(); // b superseded before it settled
  assert.equal(g.settle(b), "discard", "superseded conversion never draws");
  assert.equal(g.settle(c), "draw", "the newest conversion draws");
  assert.equal(g.settle(b), "discard", "settling twice stays discarded");
});

test("glass age: capture stamp + offset lands on the client clock", () => {
  // Offset maps capture clock -> client clock (client midpoint - host now,
  // a huge negative number across epochs). now - (capture + offset) is the
  // only sign that survives the 0..5 s plausibility gate; the old inverted
  // form fed it -3.5e9 ms and the HUD showed "-- ms e2e" forever.
  const nowUs = 120_000_000; // 120 s since page load
  const captureUs = 1_780_000_000_000_000; // host capture clock
  const offsetUs = nowUs - captureUs - 15_000; // capture happened 15 ms ago
  const age = glassAgeMs(nowUs, captureUs, offsetUs);
  assert.ok(age > 0 && age < 5000, `age ${age} ms must be plausible`);
  assert.ok(Math.abs(age - 15) < 0.001);
});

test("repairing one frame does not add pacing delay to its healthy successors", () => {
  // Exercise onFrame -> order -> pacing without DOM or codec work. The codec
  // submit and timer boundaries are replaced; the production ordering and
  // pacing methods run unchanged with actual arrival times.
  const d = Object.assign(Object.create(WtDecoder.prototype), {
    decoder: { state: "configured" },
    announced: true, pacing: true, stopped: false, paceTimer: 0,
    order: new FrameOrderer(), frames: new FrameTimeline(),
    playout: new PlayoutClock(), paced: new PacedQueue(),
    clockOffsetUs: () => null, submit: () => {},
  }) as WtDecoder;
  const oldWindow = globalThis.window;
  const oldNow = performance.now;
  let now = 0;
  const send = (no: number, delayMs = 10) => {
    const capMs = no * 1000 / 120;
    now = capMs + delayMs;
    d.onFrame({ frame_no: no, capture_us: Math.round(capMs * 1000), key: no === 0, payload: new Uint8Array(1) });
  };
  try {
    globalThis.window = { setTimeout: () => 0 } as unknown as Window & typeof globalThis;
    performance.now = () => now;
    for (let n = 0; n < 240; n++) send(n);
    for (let n = 241; n <= 263; n++) send(n);
    send(240, 210); // one repair releases a 200ms run of already-arrived frames
    assert.ok(d.pacingDelayMs <= 2, `clean route acquired ${d.pacingDelayMs}ms of artificial pacing`);
    for (let i = 0; i < 20; i++) {
      now += 1;
      d.onFrame({ frame_no: 0, capture_us: 0, key: true, payload: new Uint8Array(1) });
    }
    assert.ok(d.pacingDelayMs <= 2, "late replay copies do not count as network jitter");
  } finally {
    performance.now = oldNow;
    globalThis.window = oldWindow;
  }
});
