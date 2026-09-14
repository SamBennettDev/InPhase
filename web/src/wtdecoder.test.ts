import assert from "node:assert/strict";
import test from "node:test";
import { PresentGate, glassAgeMs } from "./wtdecoder.js";



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
