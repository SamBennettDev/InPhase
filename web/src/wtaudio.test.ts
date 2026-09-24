import { test } from "node:test";
import assert from "node:assert/strict";
import { lipSyncStartUs } from "./wtaudio.js";

// Host capture clock: frame/packet captured at host time 1_000_000 µs, which is
// local performance.now() 5_000 ms (offset = 4_000_000 µs). getOutputTimestamp
// says context time 2.0 s is being HEARD at performance.now() 5_000 ms (output
// latency is already inside that pairing).
const base = { ptsUs: 1_000_000, offsetUs: 4_000_000, contextTimeS: 2.0, performanceTimeMs: 5_000, nowUs: 2_000_000 };

test("audio is heard when its frame is seen", () => {
  // Video shows frames 20 ms after capture: heard at capture + 20 + 8 (scanout).
  const start = lipSyncStartUs({ ...base, videoAgeMs: 20 });
  assert.ok(start !== null && Math.abs(start - 2_028_000) < 1, `start ${start}`);
});

test("a target already in the past plays as early as it can", () => {
  // nowUs well past the target (audio arrived late): clamp to now + 5 ms.
  const late = lipSyncStartUs({ ...base, videoAgeMs: 20, nowUs: 2_200_000 });
  assert.equal(late, 2_205_000);
});

test("an unsynced clock yields no schedule rather than a wild one", () => {
  assert.equal(lipSyncStartUs({ ...base, offsetUs: 3_600_000_000_000, videoAgeMs: 20 }), null);
});
