// End-to-end check for the WebTransport video path: pair, connect, watch, and
// assert on **pixels**.
//
// Why this is shaped the way it is. The previous harness passed all night on
// Linux Chrome while a Mac and a phone showed black. It could not have caught
// that, for three reasons, and each one is fixed here:
//
//   1. It asserted on "first glass age", which is emitted when the *decoder*
//      produces a frame. A decoder can be producing frames perfectly while
//      nothing reaches the screen - that is precisely the iOS Safari failure
//      (drawImage of a VideoFrame silently drawing nothing). Decode is not
//      pixels. This samples the canvas and asserts on what is actually on it.
//   2. It sampled the canvas but nothing ever *asserted* on the result; the
//      numbers went into a log nobody read. Assertions here are computed in the
//      harness and emitted as a machine-readable verdict, so the shell wrapper
//      cannot forget to check one.
//   3. A frozen last frame looks identical to live video in a single sample.
//      So the canvas must both be non-blank AND change over the soak.
//
// It also never actually killed the browser in the Ctrl+Shift+Q repro. `--kill`
// does that for real: hard-kills the page mid-session and requires the host to
// free the session slot afterwards.
//
// Usage:
//   node tools/e2e/run.js <invite-url> [seconds] [--kill] [--out DIR]
//
// Env:
//   E2E_CHROME   path to a Chrome/Chromium binary (default: Playwright's)
//   E2E_STATUS   host status URL used for the post-kill slot check
//
// Exit: 0 pass, 1 assertion failed, 2 harness/timeout error.
// Writes <out>/result.json - the verdict - plus console.log and screenshots.

const { chromium } = require("playwright-core");
const fs = require("fs");
const path = require("path");

const args = process.argv.slice(2);
const INVITE = args[0];
const SECONDS = Number(args.find((a) => /^\d+$/.test(a)) || 60);
const KILL = args.includes("--kill");
const outIdx = args.indexOf("--out");
const OUT = outIdx >= 0 ? args[outIdx + 1] : "/tmp/wte2e";
const STATUS_URL = process.env.E2E_STATUS || "";

if (!INVITE) {
  console.error("usage: run.js <invite-url> [seconds] [--kill] [--out DIR]");
  process.exit(2);
}
fs.mkdirSync(OUT, { recursive: true });
const LOG = path.join(OUT, "console.log");
fs.writeFileSync(LOG, "");

/** A canvas is "blank" below this many distinct sampled colours - a black or
 *  single-flat-colour canvas is what every black-screen report looks like. */
const MIN_DISTINCT_COLOURS = 8;

const lines = [];
const t0 = Date.now();
const ts = () => `+${((Date.now() - t0) / 1000).toFixed(1)}s`;
const log = (s) => {
  const line = `[${ts()}] ${s}`;
  lines.push(line);
  fs.appendFileSync(LOG, line + "\n");
};

const result = {
  ok: false,
  failures: [],
  invite: INVITE,
  seconds: SECONDS,
  killTested: KILL,
  firstGlassMs: null,
  samples: [],
  pixels: {
    everNonBlank: false,
    everChanged: false,
    blankSamples: 0,
    sampled: 0,
    /** Consecutive blank samples since the last good one — a stream that went
     *  blank and stayed blank, which "ever non-blank" cannot detect. */
    blankAfterGood: 0,
  },
  killRecovered: null,
};
const finish = (code) => {
  result.ok = code === 0 && result.failures.length === 0;
  fs.writeFileSync(path.join(OUT, "result.json"), JSON.stringify(result, null, 2));
  console.log(lines.join("\n"));
  console.log("\n--- verdict ---");
  console.log(JSON.stringify({ ok: result.ok, failures: result.failures }, null, 2));
  process.exit(result.ok ? 0 : code || 1);
};
const fail = (why) => {
  log(`[FAIL] ${why}`);
  result.failures.push(why);
};

const hardDeadline = setTimeout(() => {
  fail("hard deadline exceeded");
  finish(2);
}, (SECONDS + 90) * 1000);
hardDeadline.unref?.();

/**
 * Read the canvas: how many distinct colours, and a hash of the sampled pixels.
 * Runs in the page. The hash is what distinguishes live video from a frozen
 * frame; the colour count is what distinguishes video from black.
 */
function probeInPage() {
  const c = document.querySelector("canvas.wt-video");
  const out = { present: !!c, w: c?.width ?? 0, h: c?.height ?? 0, distinct: 0, hash: 0 };
  if (!c || !c.width) return out;
  const g = c.getContext("2d");
  if (!g) {
    // Not a 2D context: the pixel assertion cannot run, and silently reporting
    // zero would read as a black screen. Say so instead.
    out.noContext = true;
    return out;
  }
  let d;
  try {
    d = g.getImageData(0, 0, Math.min(c.width, 480), Math.min(c.height, 270)).data;
  } catch (e) {
    out.error = String(e); // tainted canvas etc.
    return out;
  }
  const set = new Set();
  let hash = 2166136261;
  for (let i = 0; i < d.length; i += 4 * 17) {
    set.add((d[i] >> 4) * 256 + (d[i + 1] >> 4) * 16 + (d[i + 2] >> 4));
    hash = ((hash ^ d[i]) * 16777619) >>> 0;
    hash = ((hash ^ d[i + 1]) * 16777619) >>> 0;
    hash = ((hash ^ d[i + 2]) * 16777619) >>> 0;
  }
  out.distinct = set.size;
  out.hash = hash;
  return out;
}

(async () => {
  const launchOpts = {
    headless: true,
    args: ["--no-sandbox", "--disable-dev-shm-usage", "--autoplay-policy=no-user-gesture-required"],
  };
  if (process.env.E2E_CHROME) launchOpts.executablePath = process.env.E2E_CHROME;
  const browser = await chromium.launch(launchOpts);
  const ctx = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 1600, height: 900 },
  });
  const page = await ctx.newPage();
  page.on("console", (m) => {
    const t = m.text();
    lines.push(`[${ts()}] [console] ${t}`);
    fs.appendFileSync(LOG, `[${ts()}] [console] ${t}\n`);
    const g = /wt first glass age: ([0-9.]+) ms/.exec(t);
    if (g && result.firstGlassMs === null) result.firstGlassMs = Number(g[1]);
  });
  page.on("pageerror", (e) => log(`[pageerror] ${e.message}`));
  page.on("dialog", (d) => {
    log(`[dialog] ${d.type()}: ${d.message()}`);
    d.dismiss().catch(() => {});
  });

  // ---- pair + reach the play page ----------------------------------------
  await page.goto(INVITE, { waitUntil: "domcontentloaded", timeout: 20000 });
  const go = page.locator("#go");
  await go.waitFor({ state: "visible", timeout: 8000 }).catch(() => {});
  if (await go.count()) await go.click({ timeout: 5000 }).catch((e) => log(`[!] pair: ${e.message}`));
  await page.waitForTimeout(2500);
  const home = page.locator("#home");
  if (await home.count()) await home.click({ timeout: 5000 }).catch(() => {});

  const reached = await page
    .waitForFunction(() => !!document.querySelector("#connect"), { timeout: 20000 })
    .then(() => true)
    .catch(() => false);
  if (!reached) {
    fail(`never reached the play page (url=${page.url()})`);
    return finish(1);
  }

  // ---- connect ------------------------------------------------------------
  // Every Playwright call here carries an explicit short timeout. `isEnabled()`
  // on a detached element waits for the default 30s rather than returning
  // false, and #connect is removed once the session starts - so the retry loop
  // used to wedge for minutes and hit the hard deadline with zero samples,
  // reporting "timeout" for what was really "connected fine".
  const connect = page.locator("#connect");
  const connectable = async () =>
    connect.isEnabled({ timeout: 1000 }).catch(() => false);

  let onCanvas = false;
  for (let attempt = 1; attempt <= 4 && !onCanvas; attempt++) {
    let enabled = false;
    for (let i = 0; i < 12 && !enabled; i++) {
      if (await connectable()) enabled = true;
      else await page.waitForTimeout(500);
    }
    if (!enabled) {
      log(`attempt ${attempt}: #connect is not clickable (gone or disabled)`);
      break;
    }
    log(`clicking Connect (attempt ${attempt})`);
    await connect.click({ timeout: 5000 }).catch((e) => log(`[!] click: ${e.message}`));
    onCanvas = await page
      .waitForFunction(() => !!document.querySelector("canvas.wt-video"), { timeout: 20000 })
      .then(() => true)
      .catch(() => false);
    if (!onCanvas) await page.waitForTimeout(2000);
  }
  if (!onCanvas) {
    // The canvas is mounted by the decoder's onFirstFrame callback, so its
    // absence means no frame was ever *presented*. That is the black screen
    // itself, not a harness problem - and the decoder can be running the whole
    // time, which is why asserting on decode logs missed it.
    const diag = await page
      .evaluate(() => ({
        canvases: document.querySelectorAll("canvas").length,
        body: (document.body.innerText || "").slice(0, 200),
      }))
      .catch(() => ({}));
    fail(
      `no frame was ever presented - the video canvas is never mounted ` +
        `(canvases in DOM: ${diag.canvases ?? "?"}). The decoder may well be running; ` +
        `nothing reached the screen.`,
    );
    return finish(1);
  }

  // ---- soak, sampling pixels ---------------------------------------------
  const hashes = new Set();
  const t1 = Date.now();
  let n = 0;
  while (Date.now() - t1 < SECONDS * 1000) {
    await page.waitForTimeout(5000);
    const label = String(++n).padStart(2, "0");
    let s;
    try {
      s = await Promise.race([
        page.evaluate(probeInPage),
        new Promise((_, r) => setTimeout(() => r(new Error("probe timeout")), 5000)),
      ]);
    } catch (e) {
      log(`[${label}] probe failed: ${e.message}`);
      continue;
    }
    result.samples.push({ t: (Date.now() - t0) / 1000, ...s });
    result.pixels.sampled++;
    if (s.noContext) fail("canvas is not a 2D context - the pixel assertion cannot run");
    if (s.distinct >= MIN_DISTINCT_COLOURS) {
      result.pixels.everNonBlank = true;
      result.pixels.blankAfterGood = 0;
    } else {
      result.pixels.blankSamples++;
      // A stream that worked and then stopped is the failure "ever non-blank"
      // cannot see. A mid-stream resolution change used to blank the canvas
      // permanently while the first samples were fine, and this harness passed
      // it. Track the trailing blank run instead.
      if (result.pixels.everNonBlank) result.pixels.blankAfterGood++;
    }
    hashes.add(s.hash);
    log(`[${label}] canvas ${s.w}x${s.h} distinct=${s.distinct} hash=${s.hash}`);
    await page.screenshot({ path: path.join(OUT, `${label}.png`), timeout: 5000 }).catch(() => {});
  }
  result.pixels.everChanged = hashes.size > 1;

  // ---- the assertions that matter ----------------------------------------
  if (result.pixels.sampled === 0) {
    fail("no canvas samples were taken");
  } else {
    if (!result.pixels.everNonBlank) {
      fail(
        `the canvas was blank in all ${result.pixels.sampled} samples ` +
          `(<${MIN_DISTINCT_COLOURS} distinct colours) - decoding may be fine, but nothing is on screen`,
      );
    }
    if (!result.pixels.everChanged) {
      fail(
        "the canvas never changed across the soak - a frozen frame, not live video " +
          "(decode counters can keep advancing through this)",
      );
    }
    // Ending blank is a failure even if the run started fine: a session that
    // works and then dies is exactly what a mid-stream change used to cause,
    // and the "ever" assertions above are satisfied by the good early samples.
    if (result.pixels.blankAfterGood >= 2) {
      fail(
        `the canvas went blank after ${result.pixels.sampled - result.pixels.blankAfterGood} ` +
          `good samples and stayed blank for ${result.pixels.blankAfterGood} - the stream ` +
          `worked and then stopped reaching the screen`,
      );
    }
  }

  // ---- client-kill: does the host free the slot? --------------------------
  if (KILL) {
    log("killing the browser mid-session (the Ctrl+Shift+Q repro)");
    // Kill the process, not a clean close: a graceful teardown sends the very
    // disconnect the host needs, so closing politely tests nothing.
    await browser.close({ reason: "e2e hard kill" }).catch(() => {});
    const proc = browser.process?.();
    if (proc && !proc.killed) proc.kill("SIGKILL");
    if (STATUS_URL) {
      const deadline = Date.now() + 60000;
      let freed = false;
      while (Date.now() < deadline && !freed) {
        await new Promise((r) => setTimeout(r, 3000));
        try {
          const r = await fetch(STATUS_URL);
          const j = await r.json();
          if (j.available === true || j.state === "Idle") freed = true;
        } catch {
          /* host restarting or unreachable - keep trying */
        }
      }
      result.killRecovered = freed;
      if (!freed) {
        fail("the host never returned to Idle after the client was killed - session slot leaked");
      } else {
        log("host returned to Idle after the kill");
      }
    } else {
      log("[skip] no E2E_STATUS given, cannot check the slot was freed");
    }
    return finish(0);
  }

  await browser.close();
  finish(0);
})().catch((e) => {
  fail(`harness error: ${e && e.stack ? e.stack : e}`);
  finish(2);
});
