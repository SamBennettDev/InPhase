// SPDX-License-Identifier: GPL-3.0-or-later
import { test, expect } from "@playwright/test";
import { mockHost } from "./fixtures.js";

for (const pacing of [false, true]) {
  test(`real WebCodecs renders reordered frames with pacing ${pacing ? "on" : "off"}`, async ({ page }, testInfo) => {
    test.setTimeout(30_000);
    await mockHost(page);
    await page.goto("/");
    const result = await page.evaluate(async (pacing) => {
      const path = "/tests/performance/browser-stream.ts";
      const { runBrowserStream } = await import(path);
      return runBrowserStream({ width: 640, height: 360, fps: 60, durationMs: 5000, jitterMs: 25, pacing });
    }, pacing);
    await testInfo.attach("stream-metrics", { body: JSON.stringify(result, null, 2), contentType: "application/json" });
    expect(result.errors).toEqual([]);
    expect(result.lit).toBe(true);
    expect(result.stats.framesDecoded).toBeGreaterThan(250);
    expect(result.stats.framesPresented).toBeGreaterThan(150);
    expect(result.maxHeld).toBeLessThanOrEqual(64);
    expect(result.maxQueue).toBeLessThan(32);
  });
}

test("real WebCodecs resumes after a missing reference run", async ({ page }, testInfo) => {
  test.setTimeout(30_000);
  await mockHost(page);
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const path = "/tests/performance/browser-stream.ts";
    const { runBrowserStream } = await import(path);
    return runBrowserStream({ width: 640, height: 360, fps: 60, durationMs: 6000, jitterMs: 15, pacing: true, outage: [2200, 2600] });
  });
  await testInfo.attach("stream-metrics", { body: JSON.stringify(result, null, 2), contentType: "application/json" });
  expect(result.errors).toEqual([]);
  expect(result.lit).toBe(true);
  expect(result.stats.framesDecoded).toBeGreaterThan(220);
  expect(result.stats.framesPresented).toBeGreaterThan(150);
  // The fixture encoder may insert additional keyframes. Progress beyond
  // the pre-outage 132 frames proves recovery regardless of which key won.
});
