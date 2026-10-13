// SPDX-License-Identifier: GPL-3.0-or-later
// Run on an idle client. Numbers are browser-instrumented, not optical latency.
import { chromium } from "@playwright/test";
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";

const port = 5183;
const origin = `http://127.0.0.1:${port}`;
const server = spawn(process.execPath, ["node_modules/vite/bin/vite.js", "--host", "127.0.0.1", "--port", String(port), "--strictPort"], { stdio: "ignore" });
let serverError;
server.on("error", (e) => { serverError = e; });
let browser;
try {
  const deadline = Date.now() + 15_000;
  while (true) {
    if (serverError) throw serverError;
    if (server.exitCode !== null) throw new Error(`Vite exited ${server.exitCode}`);
    try { if ((await fetch(origin, { signal: AbortSignal.timeout(1000) })).ok) break; } catch {}
    if (Date.now() >= deadline) throw new Error("Vite did not start");
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  browser = await chromium.launch({ headless: true, ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH } : {}) });
  const results = [];
  for (const mode of [{ width: 640, height: 360, fps: 60 }, { width: 1920, height: 1080, fps: 60 }, { width: 2560, height: 1440, fps: 120 }]) {
    for (const pacing of [false, true]) {
      const page = await browser.newPage();
      const pageErrors = [];
      page.on("pageerror", (e) => pageErrors.push(e.message));
      await page.route(`${origin}/performance`, (route) => route.fulfill({ contentType: "text/html", body: "<!doctype html><html><body></body></html>" }));
      await page.goto(`${origin}/performance`);
      const result = await page.evaluate(async (opts) => {
        const path = "/tests/performance/browser-stream.ts";
        const { runBrowserStream } = await import(path);
        return runBrowserStream(opts);
      }, { ...mode, durationMs: 10_000, jitterMs: 25, pacing });
      results.push({ ...result, pageErrors });
      console.log(JSON.stringify({ mode, pacing, decoded: result.stats.framesDecoded, presented: result.stats.framesPresented,
        p50Ms: result.framesSummary.p50, p99Ms: result.framesSummary.p99, hitches: result.framesSummary.jank,
        pacingDelayMs: result.pacingDelayMs, errors: [...result.errors, ...pageErrors] }));
      await page.close();
    }
  }
  const output = { kind: "real-browser-synthetic-stream", browserVersion: browser.version(), results };
  if (process.argv[2]) writeFileSync(process.argv[2], JSON.stringify(output, null, 2) + "\n");
  if (results.some((r) => r.errors.length || r.pageErrors.length || !r.lit || r.stats.framesPresented === 0)) process.exitCode = 1;
} finally {
  await browser?.close();
  server.kill();
}
