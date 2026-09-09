import assert from "node:assert/strict";
import test from "node:test";
import { PresentGate } from "./wtdecoder.js";



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
