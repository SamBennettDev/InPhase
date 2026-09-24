#!/usr/bin/env node
// Cross-browser end-to-end stream check: one real browser engine (Playwright's
// Chromium, Firefox or WebKit) against a real host, sampled once a second on
// BOTH ends, then asserted.
//
//   node streaming/xbrowser.mjs --engine chromium --origin https://192.168.1.100 \
//     --admin http://127.0.0.1:47811 --duration 90 --label clean-1080p60 \
//     --settings '{"width":1920,"height":1080,"fps":60,"maxBitrateKbps":80000}'
//
// --admin is the host's loopback-only admin API, reached through an SSH
// forward (`ssh -N -L 47811:127.0.0.1:47801 user@pc`). It supplies the pairing
// PIN and - the reason it is required - the host's side of every second: the
// encoder target the bitrate controller chose, and the client telemetry the
// host actually received. "The bitrate never reaches the set mark" is a
// host-side fact; the page cannot see it.
//
// --query appends to the play URL, e.g. `--query '&render=gl'` to force the
// WebGL renderer (default: WebKit only) or `'&render=2d'` the 2D canvas.
//
// The question this answers is narrower than "did frames arrive": did the
// stream SUSTAIN itself - no second without a decoded frame, the decode rate
// held, the encoder reached the configured rate - so that anything short of
// that is the network's doing, not ours. Impairment is applied outside this
// script (netem.sh); `--profile` only labels the run and selects which checks
// are strict.
//
// A persistent profile per engine keeps one pairing per engine: every fresh
// profile enrols a new device, and the host evicts the oldest pairing past 256,
// which would eventually sign out the owner's real devices.

import { chromium, firefox, webkit } from "playwright";
import { mkdirSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const args = process.argv.slice(2);
const arg = (k, d) => {
  const i = args.indexOf(`--${k}`);
  return i >= 0 ? args[i + 1] : d;
};
const flag = (k) => args.includes(`--${k}`);

const ENGINE = arg("engine", "chromium");
const ORIGIN = arg("origin", "https://192.168.1.100").replace(/\/$/, "");
const ADMIN = arg("admin", "http://127.0.0.1:47811").replace(/\/$/, "");
const DURATION = Number(arg("duration", "90"));
const WARMUP = Number(arg("warmup", "10"));
// Seconds the controller gets to reach the configured rate on a clean route.
const RAMP_S = Number(arg("ramp", "30"));
const LABEL = arg("label", `${ENGINE}-run`);
const PROFILE = arg("profile", "clean");
const OUT = arg("out", new URL("./out/", import.meta.url).pathname);
const SETTINGS = JSON.parse(arg("settings", "{}"));
const PROFILE_DIR = arg("profile-dir", join(homedir(), ".cache", "inphase-xbrowser", ENGINE));
// Drive an already-running Chrome over the DevTools protocol instead of
// launching one - a real client machine (hardware decode, real display, real
// Wi-Fi) reached through an SSH tunnel to its --remote-debugging-port.
const CDP = arg("cdp", "");

const settings = {
  width: 1920,
  height: 1080,
  fps: 60,
  maxBitrateKbps: 80000,
  preset: "low_latency",
  streamTarget: { type: "desktop" },
  showMetrics: true,
  ...SETTINGS,
};

const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1).padStart(6)}s ${ENGINE}]`, ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function admin(path, init) {
  const r = await fetch(`${ADMIN}${path}`, { ...init, signal: AbortSignal.timeout(3000) });
  if (!r.ok) throw new Error(`${path}: HTTP ${r.status}`);
  return r.headers.get("content-type")?.includes("json") ? r.json() : r.text();
}

// Headless Chromium otherwise composites and draws the canvas in software
// (SwiftShader), which on this box costs more per 1080p frame than the frame
// interval: decode latency read 100-245 ms and the page fell behind whatever
// the stream did. These are client.sh's flags: ANGLE on Vulkan reaches the
// real GPU (RADV) and VA-API decodes. `--software` measures the slow path.
const CHROMIUM_ARGS = [
  "--autoplay-policy=no-user-gesture-required",
  ...(flag("software")
    ? []
    : [
        "--use-angle=vulkan",
        "--ignore-gpu-blocklist",
        "--enable-gpu-rasterization",
        // VA-API decode stays opt-in (--vaapi): on this Renoir it fails the
        // host's H.264 at the first frames, which tests the client's software
        // fallback rather than the stream.
        // Without it the GPU process decodes on this RADV stack, fails the
        // stream, and this Chromium reports no software H.264 decoder to fall
        // back to - so the run would measure a Linux driver, not the stream.
        ...(flag("vaapi")
          ? ["--enable-features=VaapiVideoDecoder,VaapiVideoDecodeLinuxGL,AcceleratedVideoDecodeLinuxGL"]
          : ["--disable-accelerated-video-decode"]),
        "--disable-gpu-vsync",
        "--disable-frame-rate-limit",
      ]),
];

const engines = { chromium, firefox, webkit };
if (!engines[ENGINE]) throw new Error(`unknown engine ${ENGINE}`);

// ---- host must be free ---------------------------------------------------
const status0 = await admin("/api/v1/admin/status");
if (status0.state !== "Available") {
  log(`host busy (${status0.state}) - disconnecting the previous player`);
  await admin("/api/v1/admin/disconnect", { method: "POST" }).catch(() => {});
  for (let i = 0; i < 20 && (await admin("/api/v1/admin/status")).state !== "Available"; i++) await sleep(500);
}
const PIN = arg("pin", status0.pin);

mkdirSync(OUT, { recursive: true });
const remote = CDP ? await chromium.connectOverCDP(CDP) : null;
if (!remote) mkdirSync(PROFILE_DIR, { recursive: true });
const ctx = remote
  ? remote.contexts()[0]
  : await engines[ENGINE].launchPersistentContext(PROFILE_DIR, {
  headless: !flag("headed"),
  ignoreHTTPSErrors: true,
  viewport: { width: 1280, height: 720 },
  args: ENGINE === "chromium" ? CHROMIUM_ARGS : [],
  // Headless Firefox otherwise has no WebGL and draws the canvas in software;
  // these let it use the GPU where the platform allows (see `renderer` in
  // the result for what it actually got).
  firefoxUserPrefs: flag("software")
    ? {}
    : {
        "webgl.force-enabled": true,
        "gfx.webrender.all": true,
        "gfx.canvas.accelerated": true,
        "media.ffmpeg.vaapi.enabled": true,
        "media.hardware-video-decoding.force-enabled": true,
      },
});
// Settings go in before any page script runs. Setting them after a first load
// and reloading raced a paired profile's auto-resume: the reload tore down a
// session that was still connecting.
await ctx.addInitScript((s) => {
  try {
    localStorage.setItem("inphase.settings", s);
  } catch {}
}, JSON.stringify(settings));
const page = remote ? await ctx.newPage() : (ctx.pages()[0] ?? (await ctx.newPage()));
const browserVersion = remote ? remote.version() : (ctx.browser()?.version() ?? "(persistent)");
const errors = [];
const consoleLines = [];
page.on("pageerror", (e) => errors.push({ t: Date.now() - t0, text: String(e) }));
page.on("console", (m) => {
  const text = m.text();
  // A 401 on the pairing check is how a fresh profile learns it is unpaired;
  // Chrome logs every non-2xx fetch as a console error, but it is not one.
  if (m.type() === "error" && !/status of 401/.test(text)) errors.push({ t: Date.now() - t0, text });
  if (/wt|keyframe|decoder|InPhase/i.test(text)) consoleLines.push({ t: Date.now() - t0, type: m.type(), text });
});

let result;
let gl = null;
try {
  // `?play` forces the player view (a loopback origin would get the dashboard).
  await page.goto(`${ORIGIN}/?play${arg("query", "")}`, { waitUntil: "domcontentloaded" });
  // Which rasterizer the page got: "SwiftShader"/"llvmpipe" means every canvas
  // draw is on the CPU, and the run measures that rather than the stream.
  gl = await page.evaluate(() => {
    const g = document.createElement("canvas").getContext("webgl");
    const d = g?.getExtension("WEBGL_debug_renderer_info");
    return d ? g.getParameter(d.UNMASKED_RENDERER_WEBGL) : g ? "webgl (renderer hidden)" : "no webgl";
  });
  log(`renderer: ${gl}`);

  // Pair through the real UI: the client owns its Ed25519 device key, which
  // the host requires on the pair request and on every signaling connect.
  const pinBox = page.locator("#pin");
  if (await pinBox.isVisible({ timeout: 4000 }).catch(() => false)) {
    log("pairing");
    await pinBox.fill(PIN);
    await page.locator("#go").click();
    await pinBox.waitFor({ state: "detached", timeout: 15000 });
  }
  // A returning, paired profile can resume straight into the stream; only
  // press Connect when the home screen is actually showing it.
  await page.waitForFunction(
    () => {
      const b = document.querySelector("#connect");
      return (b && !b.disabled) || document.querySelector("canvas.wt-video") || window.__inphaseWt;
    },
    null,
    { timeout: 30000 },
  );
  const connect = page.locator("#connect");
  if (await connect.isVisible().catch(() => false)) {
    log("connect");
    await connect.click();
  } else {
    log("session resumed without Connect");
  }

  // ---- sample both ends once a second -----------------------------------
  const timeline = [];
  let prev = null;
  const hist = [];
  const startAt = Date.now();
  let lastLog = 0;
  while (Date.now() - startAt < DURATION * 1000) {
    await sleep(1000);
    const now = Date.now();
    const c = await page
      .evaluate(() => {
        const w = window.__inphaseWt;
        const wire = window.__inphaseWtWire;
        const cv = document.querySelector("canvas.wt-video");
        const err = document.querySelector(".err, [role=alert]");
        return {
          audio: window.__inphaseAudio ? { ...window.__inphaseAudio } : null,
          wt: w ? { ...w } : null,
          wire: wire ? { ...wire } : null,
          w: cv?.width ?? 0,
          h: cv?.height ?? 0,
          uiError: err && err.textContent ? err.textContent.trim().slice(0, 200) : "",
        };
      })
      .catch((e) => ({ evalError: String(e) }));
    const h = await admin("/api/v1/admin/status").catch((e) => ({ adminError: String(e) }));
    // Rates over the last ~3 s of samples, not one: the page publishes its
    // counters on its own timer, so a 1 s read aliases against it - an
    // iPhone at a steady 60 fps read as 10 / 107 / 11 / 117 in alternate
    // seconds, and the dips failed runs whose stream never faltered.
    hist.push({ t: now, decoded: c.wt?.framesDecoded ?? 0, presented: c.wt?.framesPresented ?? 0 });
    if (hist.length > 4) hist.shift();
    const h0 = hist[0];
    const span = (now - h0.t) / 1000;
    const dt = prev ? (now - prev.t) / 1000 : 1;
    const decoded = c.wt?.framesDecoded ?? 0;
    const presented = c.wt?.framesPresented ?? 0;
    const received = c.wire?.framesReceived ?? 0;
    const row = {
      s: +((now - startAt) / 1000).toFixed(1),
      dec_fps: hist.length > 1 && span > 0 ? +(((c.wt?.framesDecoded ?? 0) - h0.decoded) / span).toFixed(1) : null,
      pres_fps: hist.length > 1 && span > 0 ? +(((c.wt?.framesPresented ?? 0) - h0.presented) / span).toFixed(1) : null,
      rx_fps: prev ? +((received - prev.received) / dt).toFixed(1) : null,
      // A drop in the receive counter is a new WT connection (a redial).
      redial: prev ? received < prev.received : false,
      freeze_ms: c.wt?.totalFreezeMs ?? null,
      freezes: c.wt?.freezeCount ?? null,
      dropped: c.wt?.framesDropped ?? null,
      abandoned: c.wire?.framesAbandoned ?? null,
      v4_expired: c.wire?.v4Expired ?? null,
      stream_aborted: c.wire?.streamAborted ?? null,
      repaired: c.wire?.v4Repaired ?? null,
      nacks: c.wire?.nacksSent ?? null,
      whole_nacks: c.wire?.wholeFrameNacks ?? null,
      wedged: c.wire?.streamsWedged ?? null,
      evicted: c.wire?.assemblyEvicted ?? null,
      v4_expired: c.wire?.v4Expired ?? null,
      stream_aborted: c.wire?.streamAborted ?? null,
      repaired: c.wire?.v4Repaired ?? null,
      nacks: c.wire?.nacksSent ?? null,
      whole_nacks: c.wire?.wholeFrameNacks ?? null,
      v4_expired: c.wire?.v4Expired ?? null,
      stream_aborted: c.wire?.streamAborted ?? null,
      repaired: c.wire?.v4Repaired ?? null,
      nacks: c.wire?.nacksSent ?? null,
      whole_nacks: c.wire?.wholeFrameNacks ?? null,
      decode_ms: c.wt?.decodeMs ?? null,
      e2e_ms: c.wt?.e2eMs ?? null,
      dims: c.w ? `${c.w}x${c.h}` : null,
      render: c.wt?.renderMode ?? null,
      heard_ms: c.audio?.heardAgeMs ?? null,
      seen_ms: c.audio?.seenAgeMs ?? null,
      out_lat_ms: c.audio?.outputLatencyMs ?? null,
      audio_reason: c.audio?.reason ?? null,
      audio_raw_age: c.audio?.rawAgeMs ?? null,
      ui_error: c.uiError || c.evalError || null,
      host_state: h.state ?? null,
      enc_kbps: h.stats?.host?.encoder_bitrate_kbps ?? null,
      enc_fps: h.stats?.host?.encoded_fps ?? null,
      codec: h.stats?.host?.codec ?? null,
      cli_kbps: h.stats?.client?.inbound_bitrate_kbps ?? null,
      cli_dec_fps: h.stats?.client?.decoded_fps ?? null,
      cli_p95_ms: h.stats?.client?.lat_p95_ms ?? null,
      rtt_ms: h.stats?.client?.rtt_ms ?? null,
      route_loss_pct: h.stats?.host?.route_loss_pct ?? null,
      admin_error: h.adminError ?? null,
    };
    timeline.push(row);
    prev = { t: now, decoded, presented, received };
    if (now - lastLog > 10000) {
      lastLog = now;
      log(
        `t=${row.s}s dec=${row.dec_fps}fps pres=${row.pres_fps}fps enc=${row.enc_kbps}kbps rx=${row.cli_kbps}kbps ` +
          `p95=${row.cli_p95_ms}ms dims=${row.dims} freezes=${row.freezes} aband=${row.abandoned}` +
          (row.ui_error ? ` UI:"${row.ui_error}"` : ""),
      );
    }
  }

  // ---- verdict -------------------------------------------------------------
  const target = settings.fps;
  const maxKbps = settings.maxBitrateKbps;
  const steady = timeline.filter((r) => r.s > WARMUP && r.dec_fps !== null);
  const q = (xs, p) => {
    const v = xs.filter((x) => typeof x === "number" && isFinite(x)).sort((a, b) => a - b);
    return v.length ? v[Math.min(v.length - 1, Math.floor(v.length * p))] : null;
  };
  // A stall is a whole sampled second with no decoded frame.
  const stallSecs = steady.filter((r) => r.dec_fps === 0).length;
  let longest = 0;
  let run = 0;
  for (const r of steady) {
    run = r.dec_fps === 0 ? run + 1 : 0;
    longest = Math.max(longest, run);
  }
  const lowSecs = steady.filter((r) => r.dec_fps < target * 0.8).length;
  const freezeMs =
    steady.length > 1 ? (steady.at(-1).freeze_ms ?? 0) - (steady[0].freeze_ms ?? 0) : null;
  const redials = timeline.filter((r) => r.redial).length;
  const firstAtMark = timeline.find((r) => r.enc_kbps !== null && r.enc_kbps >= maxKbps * 0.9);
  const afterMark = firstAtMark ? timeline.filter((r) => r.s >= firstAtMark.s) : [];
  const atMarkFrac = afterMark.length
    ? afterMark.filter((r) => r.enc_kbps >= maxKbps * 0.9).length / afterMark.length
    : 0;
  const decP50 = q(steady.map((r) => r.dec_fps), 0.5);
  const decodeMsP50 = q(steady.map((r) => r.decode_ms), 0.5);
  // Decode cost above the frame interval means the CLIENT'S decoder is the
  // limit (software decode on this box); fps checks are then about the client,
  // not the stream, and are reported as such rather than silently passed.
  const decoderLimited = decP50 > 0 && decodeMsP50 !== null && decodeMsP50 > 1000 / target;
  const clean = PROFILE === "clean";

  const checks = [
    { name: "frames decoded", ok: steady.some((r) => r.dec_fps > 0), detail: `${steady.length} steady seconds` },
    { name: "no stalled second", ok: stallSecs === 0, detail: `${stallSecs} s with zero decoded frames (longest ${longest} s)` },
    { name: "no WT redial", ok: redials === 0, detail: `${redials} reconnect(s)` },
    { name: "no page errors", ok: errors.length === 0, detail: errors.slice(0, 3).map((e) => e.text).join(" | ") || "none" },
    {
      name: "decode rate held",
      ok: decoderLimited ? true : clean ? lowSecs <= steady.length * 0.05 : lowSecs <= steady.length * 0.2,
      detail: `p50 ${decP50} fps of ${target}; ${lowSecs}/${steady.length} s under 80 %` +
        (decoderLimited ? ` (client decoder-limited: decode p50 ${decodeMsP50} ms > ${(1000 / target).toFixed(1)} ms frame)` : ""),
    },
    {
      name: "encoder reached the set bitrate",
      ok: clean ? !!firstAtMark && firstAtMark.s <= RAMP_S + WARMUP : true,
      detail: firstAtMark
        ? `>= 90 % of ${maxKbps} kbps at t=${firstAtMark.s}s`
        : `never; peak ${Math.max(0, ...timeline.map((r) => r.enc_kbps ?? 0))} kbps`,
    },
    {
      name: "bitrate held at the mark",
      ok: clean ? atMarkFrac >= 0.9 : true,
      detail: `${(atMarkFrac * 100).toFixed(0)} % of seconds after reaching it`,
    },
  ];
  const passed = checks.every((c) => c.ok);
  result = {
    label: LABEL,
    engine: ENGINE,
    browser: browserVersion,
    renderer: gl,
    profile: PROFILE,
    settings,
    passed,
    checks,
    measurements: {
      dec_fps_p50: decP50,
      dec_fps_p5: q(steady.map((r) => r.dec_fps), 0.05),
      pres_fps_p50: q(steady.map((r) => r.pres_fps), 0.5),
      enc_kbps_p50: q(steady.map((r) => r.enc_kbps), 0.5),
      rx_kbps_p50: q(steady.map((r) => r.cli_kbps), 0.5),
      decode_ms_p50: decodeMsP50,
      e2e_ms_p50: q(steady.map((r) => r.e2e_ms), 0.5),
      client_p95_ms_p50: q(steady.map((r) => r.cli_p95_ms), 0.5),
      freeze_ms: freezeMs,
      stall_seconds: stallSecs,
      codec: timeline.at(-1)?.codec,
      dims: timeline.at(-1)?.dims,
    },
    errors: errors.slice(0, 20),
    console: consoleLines.slice(-60),
    timeline,
  };
} catch (e) {
  result = { label: LABEL, engine: ENGINE, profile: PROFILE, passed: false, fatal: String(e), errors, console: consoleLines.slice(-60) };
  await page.screenshot({ path: join(OUT, `${LABEL}.fail.png`) }).catch(() => {});
} finally {
  await admin("/api/v1/admin/disconnect", { method: "POST" }).catch(() => {});
  if (remote) {
    // Leave the remote browser running; only this run's tab goes.
    await page.close().catch(() => {});
    await remote.close().catch(() => {});
  } else {
    await ctx.close().catch(() => {});
  }
}

writeFileSync(join(OUT, `${LABEL}.json`), JSON.stringify(result, null, 2));
if (result.fatal) log(`FATAL: ${result.fatal}`);
for (const c of result.checks ?? []) log(`${c.ok ? "PASS" : "FAIL"}  ${c.name}: ${c.detail}`);
log(result.passed ? "RESULT: PASS" : "RESULT: FAIL", `-> ${join(OUT, `${LABEL}.json`)}`);
process.exit(result.passed ? 0 : 1);
