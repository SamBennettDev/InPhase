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
  assert.equal(
    r.observe(5),
    "redial",
    "still frozen after cooldown → redial again",
  );
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
  assert.equal(
    r.observe(7),
    "reset",
    "a fresh stall re-arms the ladder (reset cooldown elapsed)",
  );
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
