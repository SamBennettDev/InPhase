import test from "node:test";
import assert from "node:assert/strict";
import { WtRecovery } from "./wtrecovery.js";

/** Fixed clock the tests drive by hand. */
function fakeClock() {
  let t = 0;
  return {
    now: () => t,
    advance: (ms: number) => {
      t += ms;
    },
  };
}

test("recovery: progress resets the stall clock and stays idle", () => {
  const c = fakeClock();
  const r = new WtRecovery({ now: c.now });
  assert.equal(r.observe(1), "none");
  c.advance(2000);
  assert.equal(r.observe(2), "none");
  c.advance(2000);
  assert.equal(r.observe(2), "none"); // only 2 s frozen — below the 3 s line
  c.advance(1500);
  assert.equal(r.observe(2), "reset"); // 3.5 s frozen → past the 3 s line
});

test("recovery: reset fires once per stall, redial after 10 s, cooldowns hold", () => {
  const c = fakeClock();
  const r = new WtRecovery({ now: c.now });
  r.observe(5);
  c.advance(3500);
  assert.equal(r.observe(5), "reset");
  c.advance(500); // 4 s frozen, 0.5 s since reset — inside cooldown
  assert.equal(r.observe(5), "none");
  c.advance(7000); // 11 s frozen — redial line, redial cooldown free
  assert.equal(r.observe(5), "redial");
  c.advance(500);
  assert.equal(r.observe(5), "none", "redial cooldown suppresses repeats");
  c.advance(21000);
  assert.equal(r.observe(5), "redial", "still frozen after cooldown → redial again");
});

test("recovery: new progress after a reset disarms the ladder", () => {
  const c = fakeClock();
  const r = new WtRecovery({ now: c.now });
  r.observe(1);
  c.advance(3500);
  assert.equal(r.observe(1), "reset");
  c.advance(500);
  assert.equal(r.observe(7), "none", "frames flow again — ladder disarmed");
  c.advance(4500);
  assert.equal(r.observe(7), "reset", "a fresh stall re-arms the ladder (reset cooldown elapsed)");
});

test("recovery: reset() forgets history", () => {
  const c = fakeClock();
  const r = new WtRecovery({ now: c.now });
  r.observe(1);
  c.advance(12000);
  assert.equal(r.observe(1), "redial");
  r.reset();
  assert.equal(r.observe(1), "none");
});

test("recovery: first presented frame after a never-decoded stall is not a reset", () => {
  // play.ts observe(0)s while the dial has not presented. The first decoded
  // picture then hudTicks with framesPresented still 0. Forgetting the stall
  // clock at that flip (WtRecovery.reset) must not immediately fire "reset"
  // — that was the 15 Sep glass-frozen-on-first-frame spiral.
  const c = fakeClock();
  const r = new WtRecovery({ now: c.now });
  assert.equal(r.observe(0), "none");
  c.advance(3500);
  assert.equal(r.observe(0), "reset", "still never-decoded → decoder reset + IDR");
  r.reset();
  assert.equal(r.observe(0), "none", "first-frame announce starts a fresh stall clock");
  c.advance(2000);
  assert.equal(r.observe(0), "none");
  c.advance(3500);
  assert.equal(r.observe(0), "reset", "a real freeze after first frame still recovers");
});

/**
 * Audit §2.5: a freeze that is waiting on a requested IDR must not be "fixed"
 * by resetting the decoder - that flushed the IDR in flight and started the
 * whole request cycle again.
 */
test("recovery: an outstanding keyframe request holds off the reset until its deadline", () => {
  const c = fakeClock();
  const r = new WtRecovery({ now: c.now, keyframeDeadlineMs: 2000 });
  r.observe(5);
  c.advance(3000);
  const askedAt = c.now();
  c.advance(500); // 3.5 s frozen, request 0.5 s old
  assert.equal(r.observe(5, { keyframeRequestedAtMs: askedAt }), "none", "the IDR is in flight");
  c.advance(1000);
  assert.equal(r.observe(5, { keyframeRequestedAtMs: askedAt }), "none", "still inside the deadline");
  c.advance(600); // request 2.1 s old: it is not coming
  assert.equal(r.observe(5, { keyframeRequestedAtMs: askedAt }), "reset");
  c.advance(6000); // 11.1 s frozen
  assert.equal(
    r.observe(5, { keyframeRequestedAtMs: c.now() - 100 }),
    "redial",
    "a fresh request never holds off the redial rung",
  );
});

test("recovery: a hidden tab suspends the ladder and restarts the stall clock", () => {
  const c = fakeClock();
  const r = new WtRecovery({ now: c.now });
  r.observe(5);
  c.advance(12_000);
  assert.equal(r.observe(5, { hidden: true }), "none", "no presents while hidden is not a freeze");
  c.advance(250);
  assert.equal(r.observe(5), "none", "back in view: a fresh clock, not 12 s of stall");
  c.advance(3500);
  assert.equal(r.observe(5), "reset", "a real freeze in view still recovers");
});
